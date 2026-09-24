# 本地 renderer 崩溃诊断

这是独立的本地诊断工具，不修改、编译或替换 launcher；不注册 new-document 注入脚本，不主动刷新、重启或关闭 Codex++。

需 Node.js 22+ 和已运行的 Codex++ 默认本机调试端口（renderer 9229，Electron 主进程 9329）。

```powershell
node tools/local-renderer-diagnostics/record.mjs
```

每 15 秒采集一次，默认 24 小时，可用 `--days=7` 延长到 7 天，或用 `--until=2026-09-30T06:30:00Z` 指定固定结束时间。恢复采集必须复用固定结束时间，避免每次重启再延长 7 天。期限届满后自动退出。日志默认写入用户目录 `.codex-session-delete/renderer-diagnostics/`，主进程和观察器每个文件上限 128 MiB，达到上限时停止并应报告给用户。`monitor-state.json` 每轮写入 PID、固定截止时间和心跳时间；外部检查以它和实际 Node PID 共同判断监测器是否健康。主进程日志可在 renderer 崩溃时继续写入。主进程重启后会重连安装，安装幂等；退出主进程自然卸载；异常杀死观察器时由外部检查重新启动。

`ensure-monitor.ps1 -Until '2026-09-30T14:30:00+08:00'` 检查实际 Node PID、固定截止时间和最近心跳；观察器丢失或日志停止更新时隐藏启动新的观察器，旧的同类进程会被清理。`watch-monitor.ps1` 可作为隐藏外部监督循环运行，每 30 秒调用一次 ensure。监督循环和观察器都不会启动 Codex++；Codex++ 退出时它们只等待并继续记录端口不可用，Codex++ 后续自行启动时再连接。Codex 关闭或电脑关机时无法采集，恢复后才可继续；这不是保证无间断的系统服务。

- `render-process-gone`：真实 reason、exitCode、最近进程内存快照，在原应用恢复监听器之前记录。
- `main-navigation-call`：reload/loadURL/loadFile 的调用位置（去掉路径、参数）；调用原方法，保留返回值。
- `renderer-navigation-requested`：脚本、刷新等导航原因，不写页面 URL。
- `renderer-sample`：JS 堆、DOM 数量/监听器数量、页面 timeOrigin、可见性、桥接最近成功时间和注入代数。
- `javascript-exception`：仅异常类型、脚本编号和行列位置，不保存异常文本。
- `process-metrics`：进程私有内存、工作集、CPU 及系统可用物理内存。

不保存聊天文本、网络正文、cookie、凭据、配置内容或页面 HTML。不上传日志。对高频事件限流；主进程崩溃记录保留优先级。诊断本身有额外开销，不使用持续 Debugger、性能 trace 或堆快照。

停止后台观察器：在输出目录创建空文件 `STOP`；它在下一轮检查时卸载并退出。重新启动之前需自行移走该文件。也可用 `node tools/local-renderer-diagnostics/record.mjs --stop` 卸载主进程钩子，但应先停止观察器，以免它重新安装。

启动外部监督循环：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File tools/local-renderer-diagnostics/watch-monitor.ps1 `
  -Until '2026-09-30T14:30:00+08:00'
```

`--once` 用于验证安装和单轮采样，保留主进程钩子至到期；`--stop` 用于显式卸载。自定义输出目录可作为第一个参数。

验证：`cargo test -p codex-plus-core --test local_renderer_diagnostics`。测试模拟 renderer OOM 和应用自动重载，验证事件顺序、幂等、数据最小化、容量限制和卸载；不实际制造崩溃。
