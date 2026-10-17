import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowRightLeft, Check, Copy, Loader2, Play, RefreshCw, Save, Square } from "lucide-react";
import { PageHeader } from "./ManagementPages";
import "./AgentCollaborationPage.css";

type Agent = { id: string; name: string; executable: string; enabled: boolean };
type CollaborationConfig = { enabled: boolean; agents: Agent[] };
type TaskStatus = "queued" | "running" | "completed" | "failed" | "cancelled" | "interrupted";
type TaskRecord = {
  id: string;
  agentId: string;
  prompt: string;
  cwd: string;
  status: TaskStatus;
  answer: string;
  error: string;
  sessionId: string;
  parentTaskId?: string | null;
  createdAtMs: number;
  updatedAtMs: number;
  cancelRequested: boolean;
  progress: string;
};
type Snapshot = {
  config: CollaborationConfig;
  tasks: TaskRecord[];
  availability?: { id: string; available: boolean; path?: string | null }[];
};
type McpConfig = { codexToml: string; claudeJson: string; command: string; args: string[] };

const defaultConfig = (): CollaborationConfig => ({
  enabled: false,
  agents: [
    { id: "codex", name: "Codex", executable: "", enabled: false },
    { id: "deepseek", name: "DeepSeek Harness", executable: "", enabled: false },
    { id: "claude", name: "Claude Code", executable: "", enabled: false },
  ],
});
const statusLabels: Record<TaskStatus, string> = {
  queued: "排队中", running: "执行中", completed: "已完成", failed: "失败", cancelled: "已取消", interrupted: "已中断",
};
const unfinished = (task: TaskRecord) => task.status === "queued" || task.status === "running";
const absolutePath = (value: string) => /^(?:\/|[a-zA-Z]:[\\/]|\\\\)/.test(value.trim());
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const copyConfig = (config: CollaborationConfig): CollaborationConfig => ({ ...config, agents: config.agents.map((agent) => ({ ...agent })) });
const formatTime = (value: number) => Number.isFinite(value) && value > 0
  ? new Date(value).toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false })
  : "—";

export function AgentCollaborationPage({ defaultCwd = "" }: { defaultCwd?: string } = {}) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [draft, setDraft] = useState<CollaborationConfig>(defaultConfig);
  const [dirty, setDirty] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [busy, setBusy] = useState("");
  const [readError, setReadError] = useState("");
  const [actionError, setActionError] = useState("");
  const [notice, setNotice] = useState("");
  const [agentId, setAgentId] = useState("codex");
  const [cwd, setCwd] = useState(defaultCwd);
  const [prompt, setPrompt] = useState("");
  const [selectedId, setSelectedId] = useState("");
  const [continueTaskId, setContinueTaskId] = useState("");
  const [mcpConfig, setMcpConfig] = useState<McpConfig | null>(null);
  const [mcpLoading, setMcpLoading] = useState(false);
  const [mcpError, setMcpError] = useState("");
  const [copied, setCopied] = useState("");
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const mounted = useRef(false);
  const lifecycle = useRef(0);
  const requestSequence = useRef(0);
  const readInFlight = useRef(false);
  const busyRef = useRef(false);
  const dirtyRef = useRef(false);
  const mcpInFlight = useRef(false);

  const applySnapshot = useCallback((next: Snapshot, saved = false) => {
    setSnapshot(next);
    // 后台轮询只更新执行状态，保留用户尚未保存的配置。
    if (saved || !dirtyRef.current) {
      setDraft(copyConfig(next.config));
      dirtyRef.current = false;
      setDirty(false);
    }
    setSelectedId((current) => next.tasks.some((task) => task.id === current)
      ? current : [...next.tasks].sort((a, b) => b.createdAtMs - a.createdAtMs)[0]?.id ?? "");
  }, []);

  const refresh = useCallback(async () => {
    if (!mounted.current || readInFlight.current || busyRef.current) return;
    const epoch = lifecycle.current;
    const sequence = ++requestSequence.current;
    readInFlight.current = true;
    setRefreshing(true);
    try {
      const next = await invoke<Snapshot>("agent_collaboration_request", { action: "snapshot", payload: {} });
      if (!mounted.current || lifecycle.current !== epoch || sequence !== requestSequence.current) return;
      applySnapshot(next);
      setReadError("");
    } catch (error) {
      if (mounted.current && lifecycle.current === epoch && sequence === requestSequence.current) setReadError(errorText(error));
    } finally {
      if (mounted.current && lifecycle.current === epoch) {
        readInFlight.current = false;
        setRefreshing(false);
      }
    }
  }, [applySnapshot]);

  useEffect(() => {
    mounted.current = true;
    lifecycle.current += 1;
    readInFlight.current = false;
    busyRef.current = false;
    mcpInFlight.current = false;
    void refresh();
    return () => {
      mounted.current = false;
      lifecycle.current += 1;
      requestSequence.current += 1;
    };
  }, [refresh]);

  const hasUnfinishedTasks = snapshot?.tasks.some(unfinished) ?? false;
  useEffect(() => {
    // 空闲时也接收其他 MCP 客户端提交的任务，发现活跃任务后缩短刷新间隔。
    const timer = window.setInterval(() => { void refresh(); }, hasUnfinishedTasks ? 2000 : 10000);
    const refreshOnFocus = () => { void refresh(); };
    window.addEventListener("focus", refreshOnFocus);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("focus", refreshOnFocus);
    };
  }, [hasUnfinishedTasks, refresh]);

  const mutate = async (action: string, payload: unknown, onSuccess?: (next: Snapshot) => void) => {
    if (!mounted.current || busyRef.current) return;
    busyRef.current = true;
    const epoch = lifecycle.current;
    // 写操作开始后，之前的只读响应即使较晚到达也不能覆盖新状态。
    const sequence = ++requestSequence.current;
    setBusy(action);
    setActionError("");
    setNotice("");
    try {
      const next = await invoke<Snapshot>("agent_collaboration_request", { action, payload });
      if (!mounted.current || lifecycle.current !== epoch || sequence !== requestSequence.current) return;
      applySnapshot(next, action === "save_config");
      setReadError("");
      onSuccess?.(next);
    } catch (error) {
      if (mounted.current && lifecycle.current === epoch) setActionError(errorText(error));
    } finally {
      if (mounted.current && lifecycle.current === epoch) {
        busyRef.current = false;
        setBusy("");
      }
    }
  };

  const editConfig = (next: CollaborationConfig) => {
    dirtyRef.current = true;
    setDirty(true);
    setDraft(next);
    setNotice("");
  };
  const editAgent = (id: string, patch: Partial<Agent>) => editConfig({ ...draft, agents: draft.agents.map((agent) => agent.id === id ? { ...agent, ...patch } : agent) });
  const saveConfig = () => {
    const invalid = draft.agents.find((agent) => agent.executable.trim() && !absolutePath(agent.executable));
    if (invalid) { setActionError(`请填写 ${invalid.name} 可执行文件的绝对路径。`); return; }
    void mutate("save_config", { ...draft, agents: draft.agents.map((agent) => ({ ...agent, executable: agent.executable.trim() })) }, () => {
      setNotice("协作配置已保存。");
      setMcpConfig(null);
      setCopied("");
    });
  };

  const tasks = [...(snapshot?.tasks ?? [])].sort((a, b) => b.createdAtMs - a.createdAtMs);
  const selected = tasks.find((task) => task.id === selectedId);
  const continuation = tasks.find((task) => task.id === continueTaskId);
  const enabledAgents = snapshot?.config.agents.filter((agent) => agent.enabled) ?? [];
  const targetId = continuation?.agentId ?? (enabledAgents.some((agent) => agent.id === agentId) ? agentId : enabledAgents[0]?.id ?? "");
  const targetAgent = enabledAgents.find((agent) => agent.id === targetId);
  const targetUnavailable = snapshot?.availability?.find((item) => item.id === targetId)?.available === false;
  const targetBusy = tasks.some((task) => task.agentId === targetId && unfinished(task));
  const targetCwd = continuation?.cwd ?? cwd;
  const formDisabledReason = !snapshot ? "正在读取协作配置。"
    : !snapshot.config.enabled ? "请先启用 Agent 协作并保存配置。"
    : !targetAgent ? "请先启用至少一个 CLI 并保存配置。"
    : targetUnavailable ? "目标 CLI 的可执行文件不可用，请检查路径并保存。"
    : targetBusy ? "此 Agent 已有排队或执行中的任务，请等待结束后再提交。"
    : continuation && (continuation.status !== "completed" || !continuation.sessionId) ? "该任务目前无法续聊，请创建新任务。"
    : "";
  const agentName = (id: string) => snapshot?.config.agents.find((agent) => agent.id === id)?.name ?? id;

  const submitTask = (event: FormEvent) => {
    event.preventDefault();
    if (busyRef.current || formDisabledReason) return;
    if (!absolutePath(targetCwd)) { setActionError("请填写工作目录的绝对路径。"); return; }
    if (!prompt.trim()) { setActionError("请填写要交给 Agent 的任务。"); return; }
    const previousIds = new Set(tasks.map((task) => task.id));
    void mutate("submit", {
      agentId: targetId,
      prompt: prompt.trim(),
      cwd: targetCwd.trim(),
      ...(continuation ? { continueTaskId: continuation.id } : {}),
    }, (next) => {
      const created = [...next.tasks].sort((a, b) => b.createdAtMs - a.createdAtMs).find((task) => !previousIds.has(task.id));
      if (created) setSelectedId(created.id);
      setPrompt("");
      setContinueTaskId("");
      setCwd(targetCwd);
      setNotice("任务已提交，可在下方查看执行状态。");
    });
  };
  const continueTask = (task: TaskRecord) => {
    setContinueTaskId(task.id);
    setAgentId(task.agentId);
    setCwd(task.cwd);
    setPrompt("");
    setActionError("");
    setNotice("");
    promptRef.current?.focus();
  };
  const loadMcpConfig = async () => {
    if (mcpInFlight.current) return;
    mcpInFlight.current = true;
    const epoch = lifecycle.current;
    setMcpLoading(true);
    setMcpError("");
    setCopied("");
    try {
      const result = await invoke<McpConfig>("agent_collaboration_request", { action: "mcp_config", payload: {} });
      if (mounted.current && lifecycle.current === epoch) setMcpConfig(result);
    } catch (error) {
      if (mounted.current && lifecycle.current === epoch) setMcpError(errorText(error));
    } finally {
      if (mounted.current && lifecycle.current === epoch) {
        mcpInFlight.current = false;
        setMcpLoading(false);
      }
    }
  };
  const copyText = async (value: string, label: string) => {
    const epoch = lifecycle.current;
    try {
      await navigator.clipboard.writeText(value);
      if (mounted.current && lifecycle.current === epoch) { setCopied(label); setMcpError(""); }
    } catch (error) {
      if (mounted.current && lifecycle.current === epoch) setMcpError(`复制失败：${errorText(error)}`);
    }
  };

  return <div className="mg-page agent-collaboration-page">
    <PageHeader icon={<ArrowRightLeft size={20} strokeWidth={1.5} />} title="Agent 协作（实验）" actions={
      <button className="mg-button" type="button" onClick={() => void refresh()} disabled={refreshing || Boolean(busy)}>
        <RefreshCw size={15} className={refreshing ? "agent-collaboration-spin" : ""} />{refreshing ? "刷新中…" : "刷新"}
      </button>
    } />
    <div className="agent-collaboration-content" aria-busy={!snapshot && refreshing}>
      <div className="agent-collaboration-intro">
        <p>把任务交给 Codex、DeepSeek Harness 或 Claude Code，在这里查看进度、读取结果和继续对话。</p>
        <div className="agent-collaboration-stages"><span>阶段 1 · CLI 任务协作</span><span data-pending="true">阶段 2 · 已有桌面会话接入：待实现</span></div>
        <p className="agent-collaboration-hint">当前会启动 CLI 执行任务；已经打开的桌面 Code 会话和自动唤醒将在后续阶段接入。</p>
      </div>

      {readError && <p className="agent-collaboration-error" role="alert">读取协作状态失败：{readError}</p>}
      {actionError && <p className="agent-collaboration-error" role="alert">操作失败：{actionError}</p>}
      {notice && <p className="agent-collaboration-notice" role="status">{notice}</p>}
      {!snapshot && refreshing && <p className="agent-collaboration-loading" role="status"><Loader2 size={15} className="agent-collaboration-spin" />正在读取协作配置…</p>}

      <section className="agent-collaboration-section" aria-labelledby="agent-collaboration-config-title">
        <div className="agent-collaboration-section-head">
          <div><h2 id="agent-collaboration-config-title">协作设置</h2><p>可填写可执行文件的绝对路径，或留空自动查找已安装的 CLI。路径检测只确认文件存在，登录状态在执行任务时验证。</p></div>
          <button type="button" className="mg-button mg-primary" disabled={!snapshot || !dirty || Boolean(busy)} onClick={saveConfig}>
            {busy === "save_config" ? <Loader2 size={15} className="agent-collaboration-spin" /> : <Save size={15} />}{busy === "save_config" ? "保存中…" : "保存配置"}
          </button>
        </div>
        <label className="agent-collaboration-enable">
          <input type="checkbox" checked={draft.enabled} disabled={!snapshot || busy === "save_config"} onChange={(event) => editConfig({ ...draft, enabled: event.target.checked })} />
          <span><strong>启用 Agent 协作</strong><small>默认关闭，保存后生效。</small></span>
          <span className="mg-badge">{snapshot?.config.enabled ? "已启用" : "未启用"}</span>
        </label>
        <div className="agent-collaboration-agents">
          {draft.agents.map((agent) => {
            const savedAgent = snapshot?.config.agents.find((item) => item.id === agent.id);
            const savedPath = savedAgent?.executable === agent.executable.trim();
            const availability = savedPath ? snapshot?.availability?.find((item) => item.id === agent.id) : undefined;
            const available = availability?.available;
            return <div className="agent-collaboration-agent" key={agent.id}>
              <label className="agent-collaboration-agent-title"><input type="checkbox" checked={agent.enabled} disabled={!snapshot || busy === "save_config"} onChange={(event) => editAgent(agent.id, { enabled: event.target.checked })} /><strong>{agent.name}</strong></label>
              <label className="mg-field" htmlFor={`agent-collaboration-path-${agent.id}`}><span>可执行文件路径（可选）</span><input id={`agent-collaboration-path-${agent.id}`} value={agent.executable} disabled={!snapshot || busy === "save_config"} placeholder={availability?.path || "留空自动查找已安装的 CLI"} spellCheck={false} autoCapitalize="off" autoCorrect="off" onChange={(event) => editAgent(agent.id, { executable: event.target.value })} /></label>
              <small className="agent-collaboration-agent-state" data-unavailable={available === false}>{!savedPath ? "路径尚未保存" : available === true ? "已找到 CLI 文件" : available === false ? "未找到 CLI 文件" : agent.executable.trim() ? "已配置，执行任务时验证" : "留空自动查找已安装的 CLI"}{availability?.path && <code>{availability.path}</code>}</small>
              {agent.id === "deepseek" && <small className="agent-collaboration-agent-note">DSH 使用 headless 配置；如提示未登录，需先配置其 CLI。</small>}
            </div>;
          })}
        </div>
        {dirty && <p className="agent-collaboration-hint">有未保存的修改。任务使用已保存的配置。</p>}
      </section>

      <section className="agent-collaboration-section" aria-labelledby="agent-collaboration-create-title">
        <div className="agent-collaboration-section-head"><div><h2 id="agent-collaboration-create-title">{continuation ? "继续对话" : "派发任务"}</h2><p>任务可以访问所选工作目录，请先完成目标 CLI 的登录。</p></div></div>
        {continuation && <div className="agent-collaboration-continuation"><div><strong>续聊 {agentName(continuation.agentId)} 的任务</strong><span>{continuation.prompt}</span><small>任务 {continuation.id} · 沿用原会话与工作目录</small></div><button type="button" className="mg-button" disabled={busy === "submit"} onClick={() => { setContinueTaskId(""); setPrompt(""); }}>返回新任务</button></div>}
        <form className="agent-collaboration-form" onSubmit={submitTask}>
          <div className="agent-collaboration-form-row">
            <label className="mg-field" htmlFor="agent-collaboration-target"><span>执行 Agent</span><select id="agent-collaboration-target" value={targetId} disabled={Boolean(continuation) || busy === "submit" || !enabledAgents.length} onChange={(event) => setAgentId(event.target.value)}>
              {continuation ? <option value={continuation.agentId}>{agentName(continuation.agentId)}</option> : enabledAgents.length ? enabledAgents.map((agent) => <option key={agent.id} value={agent.id}>{agent.name}</option>) : <option value="">请先启用一个 CLI</option>}
            </select></label>
            <label className="mg-field" htmlFor="agent-collaboration-cwd"><span>工作目录</span><input id="agent-collaboration-cwd" value={targetCwd} disabled={Boolean(continuation) || busy === "submit"} placeholder="/absolute/path/to/project" spellCheck={false} autoCapitalize="off" autoCorrect="off" onChange={(event) => setCwd(event.target.value)} /></label>
          </div>
          <label className="mg-field" htmlFor="agent-collaboration-prompt"><span>{continuation ? "追加任务或问题" : "任务内容"}</span><textarea id="agent-collaboration-prompt" ref={promptRef} rows={4} value={prompt} disabled={busy === "submit"} placeholder={continuation ? "基于刚才的结果，继续…" : "描述目标、需要检查的文件，以及希望返回的结果…"} onChange={(event) => setPrompt(event.target.value)} /></label>
          <div className="agent-collaboration-form-footer"><p className="agent-collaboration-hint" id="agent-collaboration-submit-hint">{formDisabledReason || "新任务会创建 CLI 会话。已有任务可在结果区选择“继续该任务”。"}</p><button type="submit" className="mg-button mg-primary" aria-describedby="agent-collaboration-submit-hint" disabled={Boolean(busy) || Boolean(formDisabledReason) || !prompt.trim() || !targetCwd.trim()}>{busy === "submit" ? <Loader2 size={15} className="agent-collaboration-spin" /> : <Play size={15} />}{busy === "submit" ? "提交中…" : continuation ? "发送并续聊" : "提交任务"}</button></div>
        </form>
      </section>

      <section className="agent-collaboration-section" aria-labelledby="agent-collaboration-tasks-title">
        <div className="agent-collaboration-section-head"><div><h2 id="agent-collaboration-tasks-title">任务与结果 <span className="agent-collaboration-count">{tasks.length}</span></h2><p role="status">{hasUnfinishedTasks ? "任务执行期间每 2 秒更新状态。" : "每 10 秒检查新任务，可接收其他客户端派发的任务。"}</p></div></div>
        {!tasks.length ? <div className="agent-collaboration-empty">还没有协作任务。从上方提交第一个任务。</div> : <div className="agent-collaboration-task-workspace">
          <ul className="agent-collaboration-task-list" aria-label="协作任务">
            {tasks.map((task) => <li key={task.id}><button type="button" className="agent-collaboration-task-select" aria-pressed={task.id === selectedId} onClick={() => setSelectedId(task.id)}><span className="agent-collaboration-task-title"><strong>{agentName(task.agentId)}</strong><span className="agent-collaboration-status" data-status={task.status}>{statusLabels[task.status] ?? task.status}</span></span><span className="agent-collaboration-task-prompt">{task.prompt}</span><small>{formatTime(task.createdAtMs)}{task.parentTaskId ? " · 续聊" : ""}</small></button></li>)}
          </ul>
          {selected && <div className="agent-collaboration-task-detail" key={selected.id}>
            <div className="agent-collaboration-detail-head"><div><h3>{agentName(selected.agentId)}</h3><span className="agent-collaboration-status" data-status={selected.status} role="status">{selected.cancelRequested && unfinished(selected) ? "正在取消…" : statusLabels[selected.status] ?? selected.status}</span></div><div className="agent-collaboration-detail-actions">
              {unfinished(selected) ? <button type="button" className="mg-button" disabled={Boolean(busy) || selected.cancelRequested} onClick={() => void mutate("cancel", { id: selected.id }, () => setNotice("已请求取消任务，状态将在任务停止后更新。"))}><Square size={13} />{busy === "cancel" || selected.cancelRequested ? "正在取消…" : "取消任务"}</button> : selected.status === "completed" && Boolean(selected.sessionId) && <button type="button" className="mg-button" disabled={Boolean(busy)} onClick={() => continueTask(selected)}>继续该任务</button>}
            </div></div>
            <dl className="agent-collaboration-task-meta"><dt>工作目录</dt><dd>{selected.cwd}</dd><dt>任务 ID</dt><dd>{selected.id}</dd><dt>更新时间</dt><dd>{formatTime(selected.updatedAtMs)}</dd>{selected.sessionId && <><dt>会话 ID</dt><dd>{selected.sessionId}</dd></>}{selected.parentTaskId && <><dt>接续任务</dt><dd>{selected.parentTaskId}</dd></>}</dl>
            <div className="agent-collaboration-output"><h4>任务内容</h4><pre tabIndex={0}>{selected.prompt}</pre></div>
            {selected.progress && <div className="agent-collaboration-output"><h4>执行进度</h4><pre tabIndex={0} aria-label="执行进度">{selected.progress}</pre></div>}
            {selected.answer && <div className="agent-collaboration-output"><h4>结果</h4><pre tabIndex={0} aria-label="任务结果">{selected.answer}</pre></div>}
            {selected.error && <div className="agent-collaboration-output agent-collaboration-output-error"><h4>错误信息</h4><pre tabIndex={0} aria-label="任务错误">{selected.error}</pre></div>}
            {!selected.progress && !selected.answer && !selected.error && <p className="agent-collaboration-hint">{unfinished(selected) ? "等待 CLI 返回输出…" : "该任务没有返回文本输出。"}</p>}
            {!unfinished(selected) && (selected.status !== "completed" || !selected.sessionId) && <p className="agent-collaboration-hint">只有已完成且有会话 ID 的任务可以续聊。可创建新任务继续处理。</p>}
          </div>}
        </div>}
      </section>

      <section className="agent-collaboration-section" aria-labelledby="agent-collaboration-mcp-title">
        <div className="agent-collaboration-section-head"><div><h2 id="agent-collaboration-mcp-title">从其他 Agent 调用</h2><p>通过 MCP 接入协作工具，让支持 MCP 的客户端派发任务并查询结果。</p></div><button type="button" className="mg-button" disabled={mcpLoading || !snapshot || Boolean(busy)} onClick={() => void loadMcpConfig()}>{mcpLoading ? <Loader2 size={15} className="agent-collaboration-spin" /> : <Copy size={15} />}{mcpLoading ? "读取中…" : mcpConfig ? "重新生成配置" : "生成接入配置"}</button></div>
        <p className="agent-collaboration-hint">提供 list_agents、send_task、get_task、list_tasks、cancel_task。客户端通过查询获取进度与结果；当前不支持向已有桌面会话自动推送结果。</p>
        {mcpError && <p className="agent-collaboration-error" role="alert">{mcpError}</p>}
        {copied && <p className="agent-collaboration-notice" role="status">{copied}已复制。</p>}
        {mcpConfig && <div className="agent-collaboration-mcp-configs">
          <p className="agent-collaboration-hint">将配置合并到对应客户端的 MCP 设置后重新连接。这里只复制配置。</p>
          {[{ label: "Codex 配置", value: mcpConfig.codexToml }, { label: "Claude 配置", value: mcpConfig.claudeJson }].map((item) => <div className="agent-collaboration-mcp-code" key={item.label}><div><h3>{item.label}</h3><button type="button" className="mg-button" onClick={() => void copyText(item.value, item.label)}>{copied === item.label ? <Check size={14} /> : <Copy size={14} />}复制</button></div><pre tabIndex={0} aria-label={item.label}>{item.value}</pre></div>)}
          <details className="agent-collaboration-mcp-command"><summary>其他 MCP 客户端的启动参数</summary><pre tabIndex={0}>{JSON.stringify({ command: mcpConfig.command, args: mcpConfig.args }, null, 2)}</pre></details>
        </div>}
      </section>
    </div>
  </div>;
}
