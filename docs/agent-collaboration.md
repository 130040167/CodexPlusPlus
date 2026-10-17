# Agent 协作（实验）

Codex++ 管理器最左侧的「Agent 协作」入口提供独立、默认关闭的本机协作服务。

## 第一阶段：原生 CLI 任务

- 支持 Codex、DeepSeek Harness、Claude Code；任务实际在对应 CLI 内执行。
- 保存总开关和各 Agent 的启用状态后才能派任务。
- 可执行文件路径留空时自动发现，填写时必须是绝对路径。检测到文件不代表已登录或模型可用。
- 每个 Agent 同时执行一个任务。不同 Agent 可以并行，调用方需要安排好工作目录，避免同时修改相同文件。
- 新任务创建独立会话；已完成且记录了原生会话 ID 的任务可选择「继续该任务」，续聊固定原 Agent 与工作目录。
- 提示词经 stdin 传入，不通过 shell 拼接。协作服务不会读取、复制或改写各 CLI 的凭据和供应商配置。
- 任务最长运行 10 分钟；取消会停止本次启动的 CLI，Unix 同时停止其进程组。已发生的文件或外部操作不回滚。
- 任务由发起它的管理器或 MCP 进程托管；正常退出该进程会中断其尚未完成的任务，其他客户端进程的任务不受影响。
- 中断、失败或通信结果不确定时不会自动重试；请先检查结果。

DSH headless 默认使用自己的环境凭据配置，可能无法直接使用桌面应用保存的登录。若任务报告认证失败，请先完成 DSH CLI 的配置。Codex 和 Claude Code 同样需要可用的原生登录与模型设置。

## 从其他客户端调用

在页面中生成并复制 MCP 配置。Codex 使用 TOML 片段，Claude Code 及兼容客户端可使用 `mcpServers` JSON；DSH 按其 MCP 配置格式填写同一 command/args。这里只导出配置，不自动修改其他客户端。

配置指向当前运行的 Codex++ 可执行文件，参数为：

```text
--codex-plus-agent-mcp --state-dir <Codex++ 状态目录的绝对路径>
```

该模式在 GUI 与单实例检查之前启动，只在 stdout 输出 MCP JSONL。所有客户端使用同一状态目录时共享协作开关、任务和结果。移动或删除本地构建后，需要重新生成指向有效可执行文件的配置。

工具：

| 工具 | 用途 |
| --- | --- |
| `list_agents` | 查看启用状态及 CLI 文件可用性 |
| `send_task` | 异步派发，返回 `taskId` |
| `get_task` | 按 `taskId` 查询进度、最终答案和原生会话 ID |
| `list_tasks` | 查看最近任务及活跃任务 |
| `cancel_task` | 请求停止指定任务 |

`send_task` 参数示例：

```json
{
  "agentId": "deepseek",
  "prompt": "检查当前仓库的测试失败原因，返回证据与建议。",
  "cwd": "/absolute/path/to/project"
}
```

任务提交成功仅表示已入队。调用方继续独立工作，随后使用 `get_task` 查询同一个 `taskId`；只有 `completed` 表示成功。追加任务时传入 `continueTaskId`，其值为已完成的协作任务 ID，不能填任意桌面聊天 ID。

## 存储和边界

任务与配置保存在 Codex++ 状态目录下的 `agent-collaboration.sqlite3`，独立于现有 `settings.json` 与原生会话数据库。任务记录上限 1000 条，单条提示词最多 64 KiB，最终答案最多 256 KiB；超限明确报错。界面展示活跃任务及最近历史，最多 50 条。记录不会自动删除。

第一阶段使用 CLI 创建的会话。**附着已打开的 Codex、DSH、Claude Desktop Code 会话，接收主动通知及自动唤醒属于第二阶段，目前尚未接入。**MCP 会如实返回 `desktopSessionWake: false`，查询结果通过 `get_task` 完成。

后续桌面接入层应使用明确的 host/session 身份、保留原生权限和会话状态；新增协作写能力不应直接开放到现有第三方拓展路由白名单。
