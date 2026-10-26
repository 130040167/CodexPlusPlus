# 本地网关模式

让 Claude Code、Grok 经 Codex++ 的本地 helper 使用统一的供应商池。可选开启，与原来的「直连切换」并存：
没开网关的 Agent 仍按原方式改写自己的配置文件，互不影响。

## 能做什么

- **统一供应商池**：池就是 Codex 的供应商列表（含官方登录账号），路由组就是聚合供应商。
  Claude / Grok 页里的供应商可以一键导入池中。
- **Claude Code 用任意协议的上游**：Anthropic Messages 请求在网关里转成 Responses，
  再复用现有代理链路发往 Chat / Responses / Anthropic 上游；上游本身是 Anthropic 协议时原样转发。
- **多 Key 轮换**：供应商配了多个命名 Key 时逐个展开成候选。429 冷却该 Key（按 Retry-After），
  额度耗尽冷却更久，401 停用到 Key 被改掉，连续 5xx 熔断供应商 60 秒；400/413/422 不切换。
  默认「会话粘性」，同一会话固定一个 Key，保护上游 prompt cache。
- **路由组**：复用聚合供应商的成员、策略与按模型路由规则；路由规则可额外指定「目标模型」。
  成员不认识请求的模型时自动换成成员自己的默认模型。每个路由组有独立的轮转状态，不影响 Codex。

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

- 状态文件：`<状态目录>/gateway.json`（绑定、token、选项），不进 `settings.json`。
- Claude Code：开启时写入托管供应商 `codex-plus-gateway`（`ANTHROPIC_BASE_URL` 指向网关），
  复用「关键字段」切换逻辑，hooks / permissions 等用户键不动；关闭时切回开启前的供应商。
- Grok：写入一个指向网关的 Responses 供应商；关闭时重新应用 Grok 页当前选中的供应商。
- 在 Claude / Grok 页直接切换供应商等于离开网关模式，绑定随之关闭。

## 界面

「本地网关」页分四个标签：

- **智能体**：每个 Agent 一行——状态、档位 / 模型入口、模型选择器、开关。模型选择器把路由组和
  供应商池里每个供应商的模型列在一起，左侧按供应商跳转，也可以直接输入任意模型 id。
  Agent 已开启时选完即保存；未开启时选好模型再打开开关。
- **供应商**：供应商池（含 Key 冷却 / 熔断状态与重置），「添加供应商」可从 Claude Code / Grok 导入，
  或去 Codex 供应商配置里新建。
- **路由**：每个绑定目标一张实时卡片：左边 Agent、中间网关、右边成员，最近一次请求走过的线高亮；
  下面是请求数、换上游次数、交给 Agent 的错误数，以及「上一个请求是怎么路由的」逐条说明。
  数据来自 `gateway::activity`（只在内存，重启清零），页面每 2.5 秒刷新。
- **网关**：监听地址、各 Agent 的接入地址与 token、多 Key 轮换方式、退出时恢复直连。

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
