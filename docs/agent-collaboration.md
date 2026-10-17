# 在 Codex 中使用外部子 Agent

Codex++ 最左侧的「Agent 协作」入口提供「子 Agent 设置」。日常任务在 Codex 对话里发起，管理器负责默认 Agent、CLI 路径、模型与接入配置。

## 日常用法

1. 打开「子 Agent 设置」，启用协作及需要使用的 Agent，选择默认 Agent，保存配置。
2. 点击「一键接入并刷新」。配置只管理 `codex_plus_collaboration` 条目，并同步到 Codex++ 的托管 MCP 配置，后续切换供应商仍会保留。
3. 检查独立的运行刷新状态。存在 Codex++ 原生连接时会重载本机 MCP 配置、检查子 Agent 工具并刷新 `/mcp` 的列表缓存；刷新后的工具从下一轮消息起可用。缺少连接时，先用 Codex++ 启动或接管 Codex，再点「刷新 Codex 工具」，也可在 Codex 手动重新加载 MCP。
4. 直接向 Codex 说明任务，例如：

   - 「让 DeepSeek 实现这个功能，你负责检查结果。」
   - 「让 Claude Code 审核刚才的改动。」
   - 「把这项调查交给默认子 Agent，完成后汇总给我。」

明确点名 DeepSeek、Claude Code 或另一个 Codex 时，委派到对应 Agent；未点名时才使用默认 Agent。目标不可用时不会擅自换成其他 Agent。

Codex 获得异步任务 ID 后可以继续独立工作，再通过等待或查询工具取得终态和答案，在原对话中向用户汇报。仅提交成功不能当作执行成功。

`/mcp` 面板缓存的是全局列表，原生客户端当前会缓存 5 分钟。新聊天的 MCP 就绪不一定会更新这份列表，旧聊天也不一定自动获得新工具。因此「已接入配置」「工具已发现」「列表缓存已刷新」分开显示；刷新入口会重读本机全部 MCP 配置，仅检查本服务，不会发起聊天回合或调用模型。

## 子 Agent 配置

- 支持 Codex、DeepSeek Harness、Claude Code；任务实际在对应 CLI 内执行。
- 保存总开关和各 Agent 的启用状态后才能派任务。
- 默认 Agent 仅影响未明确指定目标的委派。默认目标未启用时会报错，显式点名其他已启用目标仍然可用。
- Codex、Claude Code 可选填模型；留空沿用原生 CLI 默认模型。DSH headless 没有临时模型参数，其模型在 DSH CLI 中配置。
- 可执行文件路径留空时自动发现，填写时必须是绝对路径。检测到文件不代表已登录或模型可用。
- 每个 Agent 同时执行一个任务。不同 Agent 可以并行，调用方需要安排好工作目录，避免同时修改相同文件。
- 新任务创建独立会话；已完成且记录了原生会话 ID 的任务可选择「继续该任务」，续聊固定原 Agent 与工作目录。
- 提示词经 stdin 传入，不通过 shell 拼接。协作服务不会读取、复制或改写各 CLI 的凭据和供应商配置。
- 任务最长运行 10 分钟；取消会停止本次启动的 CLI，Unix 同时停止其进程组。已发生的文件或外部操作不回滚。
- 任务由发起它的管理器或 MCP 进程托管；正常退出该进程会中断其尚未完成的任务，其他客户端进程的任务不受影响。
- 中断、失败或通信结果不确定时不会自动重试；请先检查结果。

DSH headless 默认使用自己的环境凭据配置，可能无法直接使用桌面应用保存的登录。若任务报告认证失败，请先完成 DSH CLI 的配置。Codex 和 Claude Code 同样需要可用的原生登录与模型设置。

## 手动接入及其他客户端

「手动接入与配置导出」中保留 MCP 配置。Codex 使用 TOML 片段，Claude Code 及兼容客户端可使用 `mcpServers` JSON；DSH 按其 MCP 配置格式填写同一 command/args。只有显式点击「一键接入并刷新」才会修改 Codex 的接入配置，保存子 Agent 参数不会自动改外部客户端；单独的「刷新 Codex 工具」不写配置文件。

配置指向当前运行的 Codex++ 可执行文件，参数为：

```text
--codex-plus-agent-mcp --state-dir <Codex++ 状态目录的绝对路径>
```

该模式在 GUI 与单实例检查之前启动，只在 stdout 输出 MCP JSONL。所有客户端使用同一状态目录时共享协作开关、任务和结果。移动或删除本地构建后，需要重新生成指向有效可执行文件的配置。

工具：

| 工具 | 用途 |
| --- | --- |
| `list_agents` | 查看启用状态及 CLI 文件可用性 |
| `delegate_to_deepseek` | 用户点名 DeepSeek 时委派 |
| `delegate_to_claude_code` | 用户点名 Claude Code 时委派 |
| `delegate_to_codex` | 委派给另一个 Codex CLI |
| `delegate_task` | 可省略 `agentId`，使用默认子 Agent |
| `send_task` | 异步派发，返回 `taskId` |
| `wait_task` | 每次最多等待 30 秒，返回终态或仍在执行的状态 |
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

当前通过 MCP 把原生 CLI 作为外部子 Agent 调用，不属于 Codex 原生 `spawn_agent` 子树；不会接管已打开的其他桌面会话。MCP 会如实返回 `desktopSessionWake: false`。这些边界不影响在 Codex 对话中委派并收取结果。

后续桌面接入层应使用明确的 host/session 身份、保留原生权限和会话状态；新增协作写能力不应直接开放到现有第三方拓展路由白名单。
