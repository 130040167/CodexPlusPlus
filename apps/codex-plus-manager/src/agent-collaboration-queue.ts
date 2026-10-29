// 子 Agent 任务排队（#2772）：同一 Agent 一次只执行一个任务，繁忙时新任务排队，
// 前一个结束后按提交顺序自动启动。这里只根据快照计算提交表单需要的排队状态。

/** 与后端 MAX_QUEUED_PER_AGENT 一致；旧版快照未带上限时的兜底。 */
export const DEFAULT_MAX_QUEUED_PER_AGENT = 8;

type QueueTask = { agentId: string; status: string };

export type AgentQueueState = {
  /** 该 Agent 未结束的任务数（运行中 + 排队中），即新任务前面还有几个。 */
  active: number;
  /** 正在排队、尚未启动的任务数。 */
  queued: number;
  limit: number;
  /** 排队已满，新任务会被拒绝。 */
  full: boolean;
};

export function agentQueueState(tasks: QueueTask[], agentId: string, maxQueued?: number): AgentQueueState {
  const limit = Number.isInteger(maxQueued) && (maxQueued as number) > 0 ? maxQueued as number : DEFAULT_MAX_QUEUED_PER_AGENT;
  const own = tasks.filter((task) => task.agentId === agentId);
  const queued = own.filter((task) => task.status === "queued").length;
  const active = own.filter((task) => task.status === "queued" || task.status === "running").length;
  return { active, queued, limit, full: queued >= limit };
}
