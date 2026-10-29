# 结构化用量历史

`window.codexPlus.call("/usage/history", query)` 提供本机历史查询。它与状态栏的
`/thread-usage-live`、单会话历史和网关自身计费分开，不修改 Codex 的会话或供应商配置。
文件目录由启动进程决定，请求不能指定任意文件路径。

## 查询

普通查询返回按轮次和日期汇总的历史、分页明细和可选费用估算。加入
`includeActivity: true` 后返回结构化统计：

```javascript
const history = await window.codexPlus.call("/usage/history", {
  includeActivity: true,
  includeSessionCursors: true,
  scopeSessionId: "00000000-0000-4000-8000-000000000001",
});
```

共同筛选参数包括 `period`（`all`、`today`、`week`、`month`）、
`from` / `to`（本地日期 `YYYY-MM-DD`，包含两端）、`model`、`sessionId`、
`turnId` 和 `search`。`page` / `pageSize` 用于普通明细查询，页大小最多 500。
`includeSessionCursors` 必须是布尔值，仅显式为 `true` 时返回累计游标；
游标不随日期、模型或明细分页裁剪，仍尊重 `sessionId`。

结构化查询还支持：

- `scopeSessionId`：在 `scopedUsage` 返回该会话及其明确的子智能体后代统计；
  普通 fork 不作为子智能体。不改变全局 `days`、游标或活动记录。
- `startAt` / `endAt`：成对的毫秒时间戳，采用 `[startAt, endAt)` 半开区间。
- `activityPage`：已完成工具回执的分页，每页最多 512 条，最多 24 页。
- `activityRevision`：与上一响应 `updatedAt` 相同时，活动列表返回空并标记
  `activityPagination.unchanged`，避免反复传输。
- `liveReceipts`：核验实时回执的供应商分类，结果在 `liveAttributions` 返回；
  分类不产生额外用量。

## 响应与完整性

结构化响应包含 `accountingVersion: 2`。`days[].usage` 给出输入、缓存输入、
输出和总 token；`days[].models` 按模型汇总，`days[].breakdowns` 按模型、
供应商和 `main` / `subagent` 角色分组。未记录的推理 token 或供应商保持未知，
不以零值或当前设置猜测历史事实。

`sessionInfo` / `sessionAliases` 保留规范会话身份、迁移别名及父子关系。
`sessionCursors` 是已观测的会话累计水位，计数器重置时可以下降；
不能把它当作终身消费总量。活动回执仅包含完成的工具调用及结果，
不会返回普通用户消息或助手回复正文。
工具结果只保留成功／失败、退出状态及技能名称等分类证据，不返回正文片段。

索引在后台刷新，覆盖活动和归档目录中的 `.jsonl`、`.jsonl.zst`。
结构化明细保留最近 62 个本地日期，文件和记录数量均有上限。
首个请求、读取错误、损坏记录或达到预算时会返回 `partial`；
必须结合 `complete`、`scan`、`activityComplete` 和 `activityTruncated`
判断完整性，不应将缺失记录解释为零消费。

`ok`、`partial`、`unavailable` 均正常返回，`failed` 由扩展调用转为异常。
普通查询的 `prices` 可按模型指定每百万 token 的输入、缓存输入、输出价格，
只计算估算费用，未完整定价的项目保持未知。

## 供应商证据

供应商分类优先使用事件自身的明确记录；通用内部标签不能证明实际供应商。
请求日志和原生 HTTP 完成日志只补充归属，必须匹配会话、模型、时间及用量等证据；
双向不唯一、失败或冲突的匹配保持未知。当前配置只用于映射可观察的端点，
不会据此追溯分配配置生效之前的消费。

管理器和独立 worker 各自运行有界采集器，在自己的 Codex home 中保存
`usage-provider-cache.sqlite` 和采集锁；SQLite 日志目录可由 `CODEX_SQLITE_HOME`
单独指定。采集器不发送模型请求，不修改源日志、rollout、登录或配置文件。
落盘证据不包含 API Key、请求头或完整端点 URL，日志轮转后仍可核验已有匹配。
查询索引也会更新该派生缓存，缓存不可用时仍保留用量查询并报告恢复状态。
