# 本地网关模式

两种模式完全独立、可以随时切换：

- **直连模式**（原来的方式）：每个 Agent 有自己的供应商列表（Codex、Claude、Grok 各管各的），切换时直接改写 Agent 的配置文件。
- **网关模式**：网关有自己的一份供应商池和路由组，所有接入的 Agent 共用整个池。Agent 的配置指向 Codex++ 本地 helper，
  请求在网关里按模型名选供应商。

网关的数据只存在 `<状态目录>/gateway.json`，不读也不写 `settings.json` 里直连模式的供应商；
从直连模式「导入」是复制一份，之后两边各改各的。没接入网关的 Agent 不受任何影响。

## 能做什么

- **共用供应商池**：内置厂商预设（复用 `presets.ts`，另加 Anthropic、xAI、Ollama、LM Studio）、自定义地址，
  或从直连模式的 Codex / Claude / Grok 供应商复制。官方登录账号不能复制：refresh token 每用一次就换新，
  刷新结果只写回原来那一条，复制会让两边互相把对方刷失效，订阅账号需要在网关里单独登录（待做）。
- **按模型名路由**：裸模型名发给池里提供它的供应商，多家时按池顺序依次尝试；`<供应商 id>/<模型>` 只发给那一家；
  `group/<组 id>` 按路由组展开。
- **Claude Code 用任意协议的上游**：Anthropic Messages 请求在网关里转成 Responses，
  再复用现有代理链路发往 Chat / Responses / Anthropic 上游；上游本身是 Anthropic 协议时原样转发。
- **多 Key 轮换**：供应商填多个 Key（每行一个）时逐个展开成候选。429 冷却该 Key（按 Retry-After），
  额度耗尽冷却更久，401 停用到 Key 被改掉，连续 5xx 熔断供应商 60 秒；400/413/422 不切换。
  默认「会话粘性」，同一会话固定一个 Key，保护上游 prompt cache。
- **路由组**：网关自己的组，在「路由」页新建。成员是池里的模型（`<供应商 id>/<模型>` 或裸模型名）或别的组（可嵌套，
  出现环会跳过）。模式：依次、轮换、按权重。Agent 把组当一个模型来选。

## 结构

```
Claude Code ──POST /gw/claude/v1/messages──┐
Grok ─────────POST /gw/grok/v1/responses───┤  helper（常驻，默认 127.0.0.1:57321）
                                           │
                     gateway::server ──────┤ 鉴权（每个 Agent 一个本地 token）
                     gateway::route  ──────┤ 绑定 → 模型 → 候选（供应商 × Key）
                     gateway::pipeline ────┤ 逐个尝试；分类失败、冷却、切换
                                           │
             Anthropic 上游：原样转发 ◄────┤
             其它上游：protocol_proxy（ForcedRelay）◄─ Responses 中间格式
                                           │
             Responses SSE ──anthropic_outbound──► Anthropic SSE（Claude）
                           └─────────────────────► 原样（Grok）
```

- 状态文件：`<状态目录>/gateway.json`（接入设置、token、选项、供应商池、路由组），新建时权限 0600。
- Claude Code：开启时写入托管供应商 `codex-plus-gateway`（`ANTHROPIC_BASE_URL` 指向网关），
  复用「关键字段」切换逻辑，hooks / permissions 等用户键不动；关闭时切回开启前的供应商。
- Grok：写入一个指向网关的 Responses 供应商；关闭时重新应用 Grok 页当前选中的供应商。
- Codex++ 不再提供 Claude Code / Grok 的供应商配置页：不接入网关时，它们照自己的配置文件直连；「断开并还原」恢复接入前的配置。

## 接入方式

Agent 打开「接入」后，池里所有打开的供应商的模型和所有路由组都会出现在 Agent 自己的模型列表里
（Grok 写进 `config.toml` 的模型列表，Claude Code 通过 opus / sonnet / haiku 档位映射）。每个 Agent 可在「挑选」里
排除某些供应商。池或路由组变化后自动重写开着的 Agent 配置。

## 界面

「本地网关」页分四个标签：

- **智能体**：已接入的 Agent 在上，「未设置」的在下。每行：图标、名字和本机 CLI 版本、状态、档位 / 模型入口、模型下拉、接入开关。
  模型下拉顶部是「默认（不经 Codex++）」和「断开 Codex++（还原配置）」，左侧可切到全部 / 收藏 / 某个供应商；
  未接入时选一个模型即接入。展开后逐行显示：在 Agent 里怎么切模型、模型列表（可挑选供应商）、Claude 档位、其它选项、
  改动的配置文件（可在访达中查看）和「断开并还原」。
- **供应商**：网关池里的供应商一行一个，可拖动排序、开关、展开看模型和 Key 状态、编辑、删除；下面是「添加供应商」目录：
  订阅、供应商（厂商 API）、中转、本机四组内置预设，「导入…」从直连模式复制。
- **路由**：所有接入的 Agent 共用一张实时卡片：左边 Agent、中间网关、右边池里的供应商，最近一次请求走过的线高亮；
  下面是请求数、换上游次数、交给 Agent 的错误数，以及「上一个请求是怎么路由的」逐条说明。
  数据来自 `gateway::activity`（只在内存，重启清零），页面每 2.5 秒刷新。再往下是网关的路由组：新建、编辑、删除。
- **会话**：Codex（现有会话库）、Claude Code（`~/.claude/projects/*/*.jsonl`）、Grok（`~/.grok/sessions/session_search.sqlite`）
  的本地会话放在一起，按 Agent 筛选、搜索，一键在终端执行 Agent 自己的恢复命令。只读。
- **用量**：请求结束时按「日期 × Agent × 供应商 × 模型」累加到 `<状态目录>/gateway-usage.json`（不受请求日志裁剪影响）；
  显示 token / 缓存读取 / 推理 / 调用、按天柱状图、按供应商 / 模型 / Agent 明细。供应商填了每百万 token 价格才估算费用；
  余额只查有公开接口的 DeepSeek、Kimi / Moonshot、OpenRouter、SiliconFlow。
- **网关**：监听地址、各 Agent 的接入地址与 token、多 Key 轮换方式、退出时恢复直连、托盘快捷面板开关。

托盘快捷面板（默认开）：左键点托盘图标弹出，能给各 Agent 换模型、看最近的请求；右键仍是原菜单、双击打开主窗口。
关掉开关时托盘恢复原来的点击行为。

## 订阅登录

- **ChatGPT**：在网关里走与 Codex CLI 相同的浏览器 PKCE 登录（本机 1455 端口接回调）。token 只存在 gateway.json，
  凭据带 `codex_plus_gateway` 标记，刷新结果写回网关、不采纳 Codex 的 live 登录——与 Codex 自己的登录是两次独立的登录。
- **GitHub Copilot**：GitHub 设备码登录（VS Code Copilot 的 OAuth App），用 GitHub token 换 Copilot 的短期 token
  （约 30 分钟），请求前快过期时自动换新并写回 gateway.json；Chat Completions 协议，带 VS Code Copilot Chat 的请求头。
- **SuperGrok**：auth.x.ai 浏览器 PKCE 登录（Grok CLI 的公开客户端），推理走订阅额度的 `cli-chat-proxy.grok.com/v1`
  （Responses 协议，带 Grok CLI 的客户端标识头，版本号被 xAI 提高时会回 426，需要跟着改）；access token 快过期时用
  refresh token 换新。部分 SuperGrok 账号会被 xAI 的白名单拒绝（403），这时只能用 API Key。
- 账号类凭据都带 `codex_plus_account` / `codex_plus_gateway` 标记，与各 Agent 自己的登录互不影响。
- 没有接入：Claude 订阅（Anthropic 条款只允许官方客户端使用订阅凭据）、Gemini CLI（Google 已于 2026-06-18 停止个人账号的
  Code Assist，且其说明写明第三方使用违反政策）、Antigravity（客户端密钥未公开，只能从客户端里提取）。
- 以上登录都是冒用官方客户端身份调用未公开的接口，可能随时失效，也有账号被限制的风险。

## 限制

- 网关只在 Codex++ 运行时可用。可在「本地网关 → 网关」标签打开「退出时恢复直连」。
- 上游是 OpenAI 协议时，Anthropic 独有能力会有损失：`cache_control` 被忽略、
  服务端工具（web_search 等）只有原生 Responses 上游支持、PDF 附件取决于上游、
  思考块签名由网关隧道保存（`cpxgw1.` 前缀），换回 Anthropic 上游时会被剔除。
- ChatGPT 订阅作为 Claude Code 的上游依赖官方后端对请求的限制，需自行评估。

## 代码位置

| 模块 | 说明 |
|---|---|
| `crates/codex-plus-core/src/gateway/` | 网关全部逻辑 |
| `protocol_proxy.rs` 的 `ForcedRelay` | 让代理只发往网关指定的上游 |
| `launcher.rs` 的 `SseSink` / `ProxyOpener` | 复用 Responses 归一化链路 |
| `apps/codex-plus-manager/src-tauri/src/gateway_commands.rs` | 管理器命令 |
| `apps/codex-plus-manager/src/GatewayPage.tsx` | 「本地网关」页 |

测试：`tests/gateway_e2e.rs`（真实 helper + 模拟上游）、`tests/gateway_apply.rs`、
`tests/gateway_forced_relay.rs`，以及各模块内单元测试。
